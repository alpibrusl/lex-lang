# syntax=docker/dockerfile:1.7

# Multi-stage Rust build for the `lex` CLI / agent VCS server.
# - Stage 1 (builder): produces a release `lex` binary against pinned
#   workspace deps. Uses cargo-chef so iterative builds cache by
#   dependency manifest rather than by every source change.
# - Stage 2 (runtime): minimal Debian slim with the binary, a non-
#   root user, and a default store directory at /data.
#
# Build:    docker build -t lex-lang:latest .
# Run:      docker run --rm -p 4040:4040 -v lex-store:/data lex-lang:latest
# Compose:  see docker-compose.yml at the repo root for a full
#           Caddy + lex stack with auto-TLS.

ARG RUST_VERSION=1.94

FROM rust:${RUST_VERSION}-bookworm AS chef
WORKDIR /build
RUN cargo install cargo-chef --locked --version 0.1.71

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /build/recipe.json recipe.json
# Cache deps. Subsequent rebuilds with only source changes
# skip this layer entirely.
RUN cargo chef cook --release --recipe-path recipe.json --bin lex
COPY . .
RUN cargo build --release --bin lex
RUN strip /build/target/release/lex

FROM debian:bookworm-slim AS runtime
# tiny_http connects out for nothing — the server is purely
# inbound — so we only need TLS roots for outbound `[net]` /
# `[mcp]` calls a Lex program might make. Add ca-certificates +
# tini for clean signal handling on `docker stop`.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tini \
 && rm -rf /var/lib/apt/lists/*

# Non-root user owns /data so a host volume mount with the
# default permissions doesn't lock the server out.
RUN groupadd --system --gid 1000 lex \
 && useradd --system --uid 1000 --gid 1000 --home /data --shell /usr/sbin/nologin lex \
 && mkdir -p /data \
 && chown lex:lex /data

COPY --from=builder /build/target/release/lex /usr/local/bin/lex

USER lex
WORKDIR /data
EXPOSE 4040
VOLUME ["/data"]

# `tini` reaps zombies (matters for `agent.call_mcp` which spawns
# subprocesses) and forwards SIGTERM cleanly.
ENTRYPOINT ["/usr/bin/tini", "--", "lex"]
CMD ["serve", "--port", "4040", "--store", "/data"]
