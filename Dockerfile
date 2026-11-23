# syntax=docker/dockerfile:1

# Keep the Rust version in this tag equal to the channel in rust-toolchain.toml.
# If they differ, rustup downloads the pinned toolchain during the build.
FROM docker.io/library/rust:1.100.0-slim-trixie@sha256:6f14b2b4b278ec0d50aa1d69541c304ce0f95e0f6268487aa9924af1c2e9aebe AS builder

WORKDIR /app

# Build the dependencies first with placeholder sources, so this layer stays cached
# until Cargo.toml, Cargo.lock, or rust-toolchain.toml changes.
COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
RUN mkdir src \
    && touch src/lib.rs \
    && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
# COPY keeps the original modification times, which can be older than the placeholder
# build. Touch the crate roots so Cargo rebuilds the real sources.
RUN touch src/lib.rs src/main.rs \
    && cargo build --release --locked

# The cc image has glibc, libgcc, and the CA certificates that the platform TLS
# verifier reads from /etc/ssl/certs.
FROM gcr.io/distroless/cc-debian13:nonroot@sha256:532c7a94f9e96ba9db75eb9388f4227751131c7cb0a411d1a8521af4096135de

COPY --from=builder /app/target/release/mojang-api /usr/local/bin/mojang-api

USER nonroot:nonroot
EXPOSE 3000
# The image has no shell or curl, so the binary checks its own /health endpoint.
HEALTHCHECK --interval=30s --timeout=10s --start-period=10s --retries=3 \
    CMD ["/usr/local/bin/mojang-api", "--health-check"]
ENTRYPOINT ["/usr/local/bin/mojang-api"]
